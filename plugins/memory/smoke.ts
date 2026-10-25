// Fake core for the memory plugin. `bun run smoke.ts <agentDir> [basic|turns|idle]`; MU_MEMORY_BIN=<path> runs a compiled plugin instead of `bun run main.ts`.
// Point it at a COPY of a store, never the live one.
import { Database } from "bun:sqlite";
import * as path from "node:path";
import { computeBankScope } from "./store";

type Obj = Record<string, unknown>;
const isObj = (v: unknown): v is Obj => v !== null && typeof v === "object" && !Array.isArray(v);
const str = (o: Obj, key: string) => (typeof o[key] === "string" ? String(o[key]) : "");

const agentDir = process.argv[2];
const scenario = process.argv[3] ?? "basic";
if (!agentDir) throw new Error("usage: bun run smoke.ts <agentDir> [basic|turns|idle]");
const cwd = path.resolve(import.meta.dir, "../.."); // the mu checkout
const CANNED = "ZETA-4242 is the mu memory plugin smoke-test codename.";
const BANK_DB = path.join(agentDir, "memories", "mnemopi", "banks", computeBankScope(undefined, cwd, "per-project").bank, "mnemopi.db");

/** One plugin process speaking NDJSON; answers core/complete with a canned reply. */
class Plugin {
	readonly requests: string[] = [];
	readonly #proc = Bun.spawn(process.env.MU_MEMORY_BIN ? [process.env.MU_MEMORY_BIN] : ["bun", "run", "main.ts"], {
		cwd: import.meta.dir,
		stdin: "pipe",
		stdout: "pipe",
		stderr: "pipe",
	});
	readonly #waiters = new Map<number, PromiseWithResolvers<unknown>>();
	#nextId = 0;
	readonly #stdout: Promise<void>;

	constructor() {
		this.#stdout = this.#pump();
	}

	#send(msg: Obj): void {
		this.#proc.stdin.write(`${JSON.stringify(msg)}\n`);
		this.#proc.stdin.flush();
	}

	call(method: string, params: Obj): Promise<Obj> {
		const id = ++this.#nextId;
		const waiter = Promise.withResolvers<unknown>();
		this.#waiters.set(id, waiter);
		this.#send({ jsonrpc: "2.0", id, method, params });
		return waiter.promise.then(value => (isObj(value) ? value : {}));
	}

	async #pump(): Promise<void> {
		const decoder = new TextDecoder();
		let buf = "";
		for await (const chunk of this.#proc.stdout) {
			buf += decoder.decode(chunk, { stream: true });
			let i = buf.indexOf("\n");
			while (i >= 0) {
				this.#onLine(buf.slice(0, i));
				buf = buf.slice(i + 1);
				i = buf.indexOf("\n");
			}
		}
	}

	#onLine(line: string): void {
		if (!line.trim()) return;
		const msg: unknown = JSON.parse(line);
		if (!isObj(msg)) return;
		if (typeof msg.method === "string") {
			this.requests.push(msg.method);
			if (msg.method === "core/complete" && msg.id !== undefined) {
				const params = isObj(msg.params) ? msg.params : {};
				const system = typeof params.system === "string" ? "extraction" : "none";
				const size = typeof params.prompt === "string" ? params.prompt.length : 0;
				console.log(`  <- core/complete system=${system} prompt=${size} chars`);
				this.#send({ jsonrpc: "2.0", id: msg.id, result: { text: CANNED } });
			}
			return;
		}
		if (typeof msg.id !== "number") return;
		const waiter = this.#waiters.get(msg.id);
		if (!waiter) return;
		this.#waiters.delete(msg.id);
		if (isObj(msg.error)) waiter.reject(new Error(String(msg.error.message)));
		else waiter.resolve(msg.result);
	}

	async shutdown(): Promise<{ code: number; stderr: string }> {
		this.#send({ jsonrpc: "2.0", method: "shutdown" });
		this.#proc.stdin.end();
		const code = await this.#proc.exited;
		await this.#stdout;
		return { code, stderr: await new Response(this.#proc.stderr).text() };
	}
}

function init(sessionId: string): Obj {
	return {
		protocolVersion: 1,
		cwd,
		sessionId,
		sessionFile: `/tmp/mu-memory-test/${sessionId}.jsonl`,
		muHome: "/tmp/mu-memory-test/mu-home",
		agentDir,
	};
}

function turnMessages(k: number): Obj {
	return {
		userTurn: k,
		messages: [
			{ role: "user", content: `smoke question ${k}` },
			{ role: "assistant", content: [{ type: "text", text: `smoke answer ${k}` }] },
		],
	};
}

/** Transcript rows for one session: the retained_through_user_turn of each episode, oldest first. */
function transcriptTurns(sessionId: string): number[] {
	const db = new Database(BANK_DB, { readonly: true });
	try {
		return db
			.query<{ t: number | null }, [string]>(
				`SELECT CAST(json_extract(metadata_json, '$.retained_through_user_turn') AS INTEGER) AS t
				FROM working_memory WHERE source = 'coding-agent-transcript'
				AND json_extract(metadata_json, '$.session_id') = ? ORDER BY rowid`,
			)
			.all(sessionId)
			.map(r => r.t ?? -1);
	} finally {
		db.close();
	}
}

function show(label: string, value: unknown): void {
	console.log(`\n== ${label}\n${typeof value === "string" ? value : JSON.stringify(value, null, 2)}`);
}

if (scenario === "basic") {
	const p = new Plugin();
	show("initialize", await p.call("initialize", init("smoke-session-1")));
	show(
		"tool/call retain",
		await p.call("tool/call", {
			name: "retain",
			arguments: { items: ["The mu memory plugin smoke-test codename is ZETA-4242."] },
		}),
	);
	show(
		"tool/call recall",
		(await p.call("tool/call", { name: "recall", arguments: { query: "mu memory plugin smoke-test codename" } }))
			.content,
	);
	const prompt = await p.call("hook/prompt_submit", {
		prompt: "What is the mu memory plugin smoke-test codename?",
		userTurn: 1,
		resumed: false,
	});
	show("hook/prompt_submit context", str(prompt, "context"));
	show("hook/turn_end", await p.call("hook/turn_end", turnMessages(1)));
	show("hook/pre_compaction context", str(await p.call("hook/pre_compaction", {}), "context").slice(0, 600));
	show("hook/session_end", await p.call("hook/session_end", {}));
	const done = await p.shutdown();
	show("core saw plugin requests", p.requests);
	show("plugin exit code", done.code);
	show("plugin stderr", done.stderr || "(empty)");
} else if (scenario === "turns") {
	const sid = "smoke-turns-1";
	const a = new Plugin();
	await a.call("initialize", init(sid));
	for (const k of [1, 2, 3, 4]) {
		await a.call("hook/turn_end", turnMessages(k));
		show(`process A turn_end userTurn=${k}: transcript rows (retained_through_user_turn)`, transcriptTurns(sid));
	}
	show("process A shutdown", (await a.shutdown()).code);
	const b = new Plugin();
	await b.call("initialize", init(sid));
	await b.call("hook/turn_end", turnMessages(5));
	show("process B (restored cursor=4) turn_end userTurn=5: rows", transcriptTurns(sid));
	await b.call("hook/turn_end", turnMessages(8));
	show("process B turn_end userTurn=8 (8-4 >= 4): rows", transcriptTurns(sid));
	show("process B shutdown", (await b.shutdown()).code);
} else if (scenario === "idle") {
	const sid = "smoke-idle-1";
	const p = new Plugin();
	await p.call("initialize", init(sid));
	await p.call("hook/turn_end", turnMessages(1));
	show("idle: rows right after turn_end userTurn=1", transcriptTurns(sid));
	await Bun.sleep(4 * 60_000 + 15_000);
	show("idle: rows 4m15s later, no session_end (idle timer should have retained)", transcriptTurns(sid));
	show("idle: shutdown", (await p.shutdown()).code);
}
