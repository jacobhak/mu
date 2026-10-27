// JSON-RPC 2.0 over NDJSON on stdio, same framing as mu-common::rpc.
import { createInterface } from "node:readline";

// stdout carries the protocol; stray library logging goes to stderr.
console.log = console.error;
console.info = console.error;
console.debug = console.error;

export type Obj = Record<string, unknown>;

export function isObj(v: unknown): v is Obj {
	return v !== null && typeof v === "object" && !Array.isArray(v);
}

export class RpcError extends Error {
	constructor(
		readonly code: number,
		message: string,
	) {
		super(message);
	}
}

export interface Client {
	/** `opts.timeoutMs` rejects the call and forgets its pending entry, so a lost reply cannot leak it. */
	call(method: string, params?: unknown, opts?: { timeoutMs?: number }): Promise<unknown>;
	notify(method: string, params?: unknown): void;
}

export type Handler = (method: string, params: Obj, client: Client) => Promise<unknown>;

export function log(message: string): void {
	process.stderr.write(`[memory] ${message}\n`);
}

export interface Transport {
	input: NodeJS.ReadableStream;
	write(line: string): void;
}

const stdio: Transport = { input: process.stdin, write: line => void process.stdout.write(line) };

/** Serves core requests until `shutdown` or input EOF. Requests run concurrently; resolves once they settle. */
export async function serve(handler: Handler, io: Transport = stdio): Promise<void> {
	const pending = new Map<number, PromiseWithResolvers<unknown>>();
	const tasks = new Set<Promise<void>>();
	let nextId = 0;
	let closed = false;
	const write = (msg: object) => io.write(`${JSON.stringify(msg)}\n`);
	const client: Client = {
		call(method, params = {}, opts) {
			if (closed) return Promise.reject(new RpcError(-32000, "core connection closed"));
			const id = ++nextId;
			const waiter = Promise.withResolvers<unknown>();
			pending.set(id, waiter);
			write({ jsonrpc: "2.0", id, method, params });
			const ms = opts?.timeoutMs;
			if (!ms || ms <= 0) return waiter.promise;
			const timer = setTimeout(() => {
				if (pending.delete(id)) waiter.reject(new RpcError(-32000, `${method} timed out after ${ms}ms`));
			}, ms);
			return waiter.promise.finally(() => clearTimeout(timer));
		},
		notify(method, params = {}) {
			write({ jsonrpc: "2.0", method, params });
		},
	};
	for await (const line of createInterface({ input: io.input, crlfDelay: Infinity })) {
		let parsed: unknown;
		try {
			parsed = JSON.parse(line);
		} catch {
			continue;
		}
		if (!isObj(parsed)) continue;
		const { id, method, params, result, error } = parsed;
		if (typeof method !== "string") {
			const waiter = typeof id === "number" ? pending.get(id) : undefined;
			if (!waiter || typeof id !== "number") continue;
			pending.delete(id);
			if (isObj(error)) {
				waiter.reject(
					new RpcError(
						typeof error.code === "number" ? error.code : -32000,
						typeof error.message === "string" ? error.message : "error",
					),
				);
			} else waiter.resolve(result ?? null);
			continue;
		}
		if (method === "shutdown") {
			if (id !== undefined) write({ jsonrpc: "2.0", id, result: {} });
			break;
		}
		const task = (async () => {
			try {
				const out = await handler(method, isObj(params) ? params : {}, client);
				if (id !== undefined) write({ jsonrpc: "2.0", id, result: out ?? {} });
			} catch (e) {
				const message = e instanceof Error ? e.message : String(e);
				if (id !== undefined) {
					const code = e instanceof RpcError ? e.code : -32000;
					write({ jsonrpc: "2.0", id, error: { code, message } });
				} else log(`${method}: ${message}`);
			}
		})().catch(() => {});
		tasks.add(task);
		void task.then(() => tasks.delete(task));
	}
	closed = true;
	for (const waiter of pending.values()) waiter.reject(new RpcError(-32000, "core connection closed"));
	pending.clear();
	await Promise.all(tasks);
}
