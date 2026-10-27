// One mu session's memory state: mnemopi store, retain buffer and the JSON-RPC methods of the plugin protocol.
// Stdio mode runs exactly one; the shared daemon runs one per connected `mu` (see daemon.ts).
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
import { type Client, type Obj, RpcError, isObj, log } from "./rpc";
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

export class MemorySession {
	#session: { cwd: string; sessionId: string; cfg: MemoryConfig } | undefined;
	#store: Store | undefined;
	#buffer: Msg[] = [];
	#lastRetained = 0;
	#lastUserTurn = 0;
	#idleTimer: Timer | undefined;
	#client: Client | undefined;

	/** Mnemopi's LLM hook: fact extraction and consolidation go through the core's `memory` role chain. */
	#complete: Complete = async (prompt, opts) => {
		const task = opts?.task;
		const req = task?.kind === "memory-extraction" ? { system: EXTRACTION_PROMPT, prompt: task.input } : { prompt };
		try {
			if (!this.#client) throw new Error("no core connection");
			const res = await this.#client.call(
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

	/** Entry point for `serve`: remembers the core connection used for `core/complete`, then dispatches. */
	handle = async (method: string, params: Obj, client: Client): Promise<unknown> => {
		this.#client = client;
		if (method === "initialize") return this.#initialize(params);
		if (method === "tool/call") return this.#toolCall(params);
		if (method.startsWith("hook/")) return this.#hook(method.slice("hook/".length), params);
		throw new RpcError(-32601, `method not found: ${method}`);
	};

	#getStore(): Store {
		if (!this.#session) throw new Error("memory plugin not initialized");
		if (!this.#store) {
			const { cfg, sessionId, cwd } = this.#session;
			this.#store = openStore(cfg, sessionId, cwd, this.#complete);
			this.#lastRetained = Math.max(this.#lastRetained, restoreCursor(this.#store));
		}
		return this.#store;
	}

	#retainBuffer(s: Store, sourceId: string): void {
		const msgs = this.#buffer;
		this.#buffer = [];
		retainTurns(s, msgs, sourceId, this.#lastUserTurn);
		this.#lastRetained = this.#lastUserTurn;
	}

	/** Retains pending turns, flushes extraction and closes the store. The next hook reopens it lazily. */
	async endSession(): Promise<void> {
		clearTimeout(this.#idleTimer);
		this.#idleTimer = undefined;
		const current = this.#store;
		this.#store = undefined;
		if (current) {
			try {
				if (this.#buffer.length > 0) this.#retainBuffer(current, current.sessionId);
				await flushStore(current);
			} catch (e) {
				log(`session end: ${errMsg(e)}`);
			} finally {
				closeStore(current);
			}
		}
		this.#buffer = [];
		this.#lastRetained = 0;
		this.#lastUserTurn = 0;
	}

	/** Retains the buffer before the host's idle shutdown (plugin lifetime idle:5m) can drop it. */
	#armIdle(): void {
		clearTimeout(this.#idleTimer);
		this.#idleTimer = setTimeout(() => {
			this.#idleTimer = undefined;
			this.#idleRetain().catch(e => log(`idle retention failed: ${errMsg(e)}`));
		}, IDLE_RETAIN_MS);
	}

	async #idleRetain(): Promise<void> {
		const store = this.#store;
		if (!store) return;
		if (this.#buffer.length > 0) this.#retainBuffer(store, `${store.sessionId}-${Date.now()}`);
		await flushStore(store);
	}

	async #initialize(p: Obj): Promise<Obj> {
		const { cwd, sessionId, agentDir } = p;
		if (typeof cwd !== "string" || typeof sessionId !== "string" || typeof agentDir !== "string") {
			throw new RpcError(-32602, "initialize needs cwd, sessionId and agentDir");
		}
		await this.endSession();
		this.#session = { cwd, sessionId, cfg: loadConfig(agentDir) };
		return {};
	}

	async #promptSubmit(p: Obj): Promise<Obj> {
		if (!this.#session || p.resumed === true || p.userTurn !== 1) return {};
		const prompt = typeof p.prompt === "string" ? p.prompt.trim() : "";
		if (!prompt) return {};
		const s = this.#getStore();
		// First turn of a fresh session has no history, so the query is the prompt alone.
		const query = truncateCodePoints(prompt, s.cfg.recallMaxQueryChars);
		const results = await collectRecall(s, query);
		const block = results.length > 0 ? `\n\n${formatRecallBlock(results)}` : "";
		return { context: truncateApproxTokens(`${INSTRUCTIONS}${block}`, s.cfg.injectionTokenLimit) };
	}

	async #turnEnd(p: Obj): Promise<Obj> {
		if (!this.#session) return {};
		const s = this.#getStore();
		if (typeof p.userTurn === "number") this.#lastUserTurn = p.userTurn;
		this.#buffer.push(...wireMessages(p.messages));
		if (this.#lastUserTurn - this.#lastRetained >= s.cfg.retainEveryNTurns) {
			this.#retainBuffer(s, `${s.sessionId}-${Date.now()}`);
		}
		this.#armIdle();
		return {};
	}

	async #preCompaction(): Promise<Obj> {
		if (!this.#session || this.#buffer.length === 0) return {};
		const s = this.#getStore();
		const last = this.#buffer.findLast(m => m.role === "user");
		if (!last) return {};
		const query = truncateRecallQuery(
			composeRecallQuery(last.content, this.#buffer, s.cfg.recallContextTurns),
			last.content,
			s.cfg.recallMaxQueryChars,
		);
		const results = await collectRecall(s, query);
		if (results.length === 0) return {};
		return { context: truncateApproxTokens(formatRecallBlock(results), s.cfg.injectionTokenLimit) };
	}

	async #recallTool(s: Store, args: Obj): Promise<Obj> {
		const query = typeof args.query === "string" ? args.query.trim() : "";
		if (!query) return { content: "query must be a non-empty string", isError: true };
		const results = await collectRecall(s, query);
		if (results.length === 0) return { content: "No relevant memories found." };
		const n = results.length;
		return {
			content: `Found ${n} relevant ${n === 1 ? "memory" : "memories"} (as of ${formatCurrentTime()} UTC):\n\n${formatRecallList(results)}`,
		};
	}

	#retainTool(s: Store, args: Obj): Obj {
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

	async #toolCall(p: Obj): Promise<Obj> {
		try {
			const s = this.#getStore();
			const args = isObj(p.arguments) ? p.arguments : {};
			if (p.name === "recall") return await this.#recallTool(s, args);
			if (p.name === "retain") return this.#retainTool(s, args);
			return { content: `unknown tool: ${String(p.name)}`, isError: true };
		} catch (e) {
			return { content: errMsg(e), isError: true };
		}
	}

	async #hook(name: string, p: Obj): Promise<Obj> {
		try {
			switch (name) {
				case "prompt_submit":
					return await this.#promptSubmit(p);
				case "turn_end":
					return await this.#turnEnd(p);
				case "pre_compaction":
					return await this.#preCompaction();
				case "session_end":
					await this.endSession();
					return {};
				default:
					return {};
			}
		} catch (e) {
			log(`hook ${name} failed: ${errMsg(e)}`);
			return {};
		}
	}
}
