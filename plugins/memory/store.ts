// Mnemopi store access: config, bank scoping, recall and retention. Ported from omp mnemopi/config.ts and mnemopi/state.ts (MIT).
import { Database } from "bun:sqlite";
import { type Dirent, readFileSync, readdirSync } from "node:fs";
import * as path from "node:path";
import { Mnemopi, type RecallResult } from "@oh-my-pi/pi-mnemopi";
import type { RememberFacadeOptions } from "@oh-my-pi/pi-mnemopi/core/memory";
import { BankManager } from "@oh-my-pi/pi-mnemopi/core";
import { parse } from "yaml";
import { type Msg, embeddableTranscript, frameTranscript } from "./content";
import { redactRememberWrite } from "./redact";
import { type Obj, isObj, log } from "./rpc";

export type Scoping = "global" | "per-project" | "per-project-tagged";

export interface MemoryConfig {
	dbPath: string;
	bank?: string;
	scoping: Scoping;
	noEmbeddings: boolean;
	embeddingModel: string;
	debug: boolean;
	proactiveLinking: boolean;
	polyphonicRecall: boolean;
	enhancedRecall: boolean;
	retainEveryNTurns: number;
	recallLimit: number;
	recallContextTurns: number;
	recallMaxQueryChars: number;
	injectionTokenLimit: number;
}

export type Complete = (
	prompt: string,
	opts?: { maxTokens?: number; timeout?: number; task?: { kind: string; input: string } },
) => Promise<string | null>;

export interface Target {
	bank: string;
	memory: Mnemopi;
}

export interface Store {
	sessionId: string;
	cwd: string;
	cfg: MemoryConfig;
	retain: Target;
	recall: Target[];
	global?: Target;
	owned: Mnemopi[];
}

const SCOPINGS: readonly Scoping[] = ["global", "per-project", "per-project-tagged"];
const DEFAULT_BANK = "default";
const LEGACY_BANK_SCAN_LIMIT = 64;

function num(v: unknown, fallback: number, min: number): number {
	return typeof v === "number" && Number.isFinite(v) ? Math.max(min, Math.floor(v)) : fallback;
}

/** Reads the `mnemopi:` section of `<agentDir>/config.yml`; missing or invalid input falls back to defaults. */
export function loadConfig(agentDir: string): MemoryConfig {
	let raw: Obj = {};
	try {
		const doc: unknown = parse(readFileSync(path.join(agentDir, "config.yml"), "utf8"));
		if (isObj(doc) && isObj(doc.mnemopi)) raw = doc.mnemopi;
	} catch (e) {
		const missing = e instanceof Error && "code" in e && e.code === "ENOENT";
		if (!missing) log(`config.yml ignored: ${String(e)}`);
	}
	const scoping = SCOPINGS.find(s => s === raw.scoping) ?? "per-project";
	const variant = raw.embeddingVariant === "multilingual" ? "multilingual" : "en";
	const override = typeof raw.embeddingModel === "string" ? raw.embeddingModel.trim() : "";
	const dbOverride = typeof raw.dbPath === "string" ? raw.dbPath.trim() : "";
	const bank = typeof raw.bank === "string" ? raw.bank : undefined;
	return {
		dbPath: dbOverride || path.join(agentDir, "memories", "mnemopi", "mnemopi.db"),
		bank,
		scoping,
		noEmbeddings: raw.noEmbeddings === true,
		embeddingModel:
			override ||
			(variant === "multilingual" ? "intfloat/multilingual-e5-large" : "BAAI/bge-base-en-v1.5"),
		debug: raw.debug === true,
		proactiveLinking: raw.proactiveLinking === true,
		polyphonicRecall: raw.polyphonicRecall === true,
		enhancedRecall: raw.enhancedRecall === true,
		retainEveryNTurns: num(raw.retainEveryNTurns, 4, 1),
		recallLimit: num(raw.recallLimit, 8, 1),
		recallContextTurns: num(raw.recallContextTurns, 3, 1),
		recallMaxQueryChars: num(raw.recallMaxQueryChars, 4000, 256),
		injectionTokenLimit: num(raw.injectionTokenLimit, 5000, 256),
	};
}

function sanitizeBank(value: string | undefined): string | undefined {
	const raw = value?.trim();
	if (!raw) return undefined;
	const sanitized = raw.replace(/[^a-zA-Z0-9_-]+/g, "-").replace(/^-+|-+$/g, "");
	return sanitized ? limitBank(sanitized) : undefined;
}

function limitBank(name: string): string {
	if (name.length <= 64) return name;
	const hash = Bun.hash(name).toString(36);
	const prefix = name.slice(0, Math.max(1, 63 - hash.length)).replace(/-+$/g, "") || "bank";
	return `${prefix}-${hash}`;
}

function projectBank(configured: string | undefined, cwd: string): string {
	const root = path.resolve(cwd || ".");
	const project = sanitizeBank(path.basename(root)) ?? "default";
	const segment = limitBank(`${project}-${Bun.hash(root).toString(36)}`);
	const base = sanitizeBank(configured);
	return limitBank(base ? `${base}-${segment}` : segment);
}

interface BankScope {
	bank: string;
	globalBank: string;
	recallBanks: string[];
}

/** Write bank, shared bank and recall banks for a scoping mode (mirrors omp `computeMnemopiBankScope`). */
export function computeBankScope(configured: string | undefined, cwd: string, scoping: Scoping): BankScope {
	const project = projectBank(configured, cwd);
	const shared = sanitizeBank(configured) ?? DEFAULT_BANK;
	switch (scoping) {
		case "global":
			return { bank: shared, globalBank: shared, recallBanks: [shared] };
		case "per-project":
			return { bank: project, globalBank: shared, recallBanks: [project] };
		case "per-project-tagged":
			return { bank: project, globalBank: shared, recallBanks: project === shared ? [project] : [project, shared] };
	}
}

const LEGACY_PROBE = `SELECT CASE
	WHEN EXISTS(SELECT 1 FROM working_memory WHERE json_extract(metadata_json, '$.cwd') IS NOT ?1) THEN 0
	ELSE EXISTS(SELECT 1 FROM working_memory WHERE json_extract(metadata_json, '$.cwd') = ?1)
END AS safe`;

/** True when every row of a bank was written from `cwd` (and at least one was). */
function bankOnlyHasCwd(dbPath: string, cwd: string): boolean {
	let db: Database | undefined;
	try {
		db = new Database(dbPath, { readonly: true });
		const row = db.query<{ safe: number }, [string]>(LEGACY_PROBE).get(cwd);
		return row?.safe === 1;
	} catch {
		return false;
	} finally {
		try {
			db?.close();
		} catch {
			// read-only handle
		}
	}
}

/** Adds single-cwd banks left by older bank derivations to the recall set. */
function extendWithLegacyBanks(resolved: string[], dbPath: string, cwd: string): string[] {
	const banksDir = path.join(path.dirname(dbPath), "banks");
	let entries: Dirent[];
	try {
		entries = readdirSync(banksDir, { withFileTypes: true });
	} catch {
		return resolved;
	}
	const have = new Set(resolved);
	const cwdAbs = path.resolve(cwd || ".");
	const extras: string[] = [];
	let scanned = 0;
	for (const entry of entries) {
		if (!entry.isDirectory() || have.has(entry.name)) continue;
		if (scanned++ >= LEGACY_BANK_SCAN_LIMIT) break;
		if (bankOnlyHasCwd(path.join(banksDir, entry.name, "mnemopi.db"), cwdAbs)) extras.push(entry.name);
	}
	return extras.length === 0 ? resolved : [...resolved, ...extras];
}

/** Opens the write bank, recall banks and (tagged scoping) the shared bank. */
export function openStore(
	cfg: MemoryConfig,
	sessionId: string,
	cwd: string,
	complete: Complete,
): Store {
	const scope = computeBankScope(cfg.bank, cwd, cfg.scoping);
	const recallBanks =
		cfg.scoping === "global" ? scope.recallBanks : extendWithLegacyBanks(scope.recallBanks, cfg.dbPath, cwd);
	const banks = new BankManager(path.dirname(cfg.dbPath));
	const opened = new Map<string, Target>();
	const open = (bank: string): Target => {
		const existing = opened.get(bank);
		if (existing) return existing;
		const dbPath = bank === scope.globalBank ? cfg.dbPath : banks.getBankDbPath(bank);
		const target = {
			bank,
			memory: new Mnemopi({
				dbPath,
				bank,
				sessionId: bank,
				authorId: "coding-agent",
				authorType: "agent",
				channelId: bank,
				noEmbeddings: cfg.noEmbeddings,
				debug: cfg.debug,
				embeddingModel: cfg.embeddingModel,
				llm: complete,
				proactiveLinking: cfg.proactiveLinking,
				polyphonicRecall: cfg.polyphonicRecall,
				enhancedRecall: cfg.enhancedRecall,
			}),
		};
		opened.set(bank, target);
		return target;
	};
	const retain = open(scope.bank);
	const recall = recallBanks.map(open);
	const global = cfg.scoping === "per-project-tagged" ? open(scope.globalBank) : undefined;
	return {
		sessionId,
		cwd,
		cfg,
		retain,
		recall,
		global,
		owned: [...opened.values()].map(t => t.memory),
	};
}

export function closeStore(store: Store): void {
	for (const memory of store.owned) memory.close();
}

export async function flushStore(store: Store): Promise<void> {
	await Promise.all(store.owned.map(memory => memory.flushExtractions()));
}

/** Turns already retained for this session, read from the retain bank's transcript rows. */
export function restoreCursor(store: Store): number {
	const rows = store.retain.memory.db
		.query<{ content: string; sourceId: string | null; retainedThroughUserTurn: number | null }, [string]>(
			`SELECT content,
				json_extract(metadata_json, '$.source_id') AS sourceId,
				CAST(json_extract(metadata_json, '$.retained_through_user_turn') AS INTEGER) AS retainedThroughUserTurn
			FROM working_memory
			WHERE source = 'coding-agent-transcript'
				AND json_extract(metadata_json, '$.session_id') = ?
			ORDER BY rowid`,
		)
		.all(store.sessionId);
	let cursor = 0;
	for (const row of rows) {
		if (Number.isInteger(row.retainedThroughUserTurn)) {
			cursor = Math.max(cursor, row.retainedThroughUserTurn ?? 0);
			continue;
		}
		if (row.sourceId !== store.sessionId && !row.sourceId?.startsWith(`${store.sessionId}-`)) continue;
		cursor = Math.max(cursor, row.content.split(/\r?\n/).filter(line => line === "[role: user]").length);
	}
	return cursor;
}

export function globalTarget(store: Store): Target {
	const target = store.cfg.scoping === "global" ? store.retain : store.global;
	if (!target) throw new Error("Mnemopi global scope requires global or per-project-tagged scoping.");
	return target;
}

export function rememberScoped(
	store: Store,
	content: string,
	options: RememberFacadeOptions,
	target: Target = store.retain,
): string {
	const [scrubbed, scrubbedOptions] = redactRememberWrite(content, options);
	return target.memory.remember(scrubbed, scrubbedOptions);
}

/** Writes one transcript episode (background: failures are logged, not thrown) and runs fact extraction on user text. */
export function retainTurns(store: Store, messages: Msg[], sourceId: string, userTurn: number): void {
	const { transcript, messageCount } = frameTranscript(messages);
	if (!transcript) return;
	const userText = frameTranscript(messages.filter(m => m.role === "user")).transcript;
	const extract = userText !== null;
	try {
		rememberScoped(
			store,
			transcript,
			{
				source: "coding-agent-transcript",
				importance: 0.65,
				metadata: {
					session_id: store.sessionId,
					source_id: sourceId,
					message_count: messageCount,
					retained_through_user_turn: userTurn,
					cwd: store.cwd,
				},
				scope: "bank",
				extract,
				extractEntities: extract,
				extractText: extract ? userText : null,
				embedText: embeddableTranscript(messages).transcript,
				veracity: "unknown",
				memoryType: "episode",
			},
		);
	} catch (e) {
		log(`retain failed: ${String(e)}`);
	}
}

function mergeRecall(
	merged: RecallResult[],
	byId: Map<string, number>,
	byContent: Map<string, number>,
	result: RecallResult,
): void {
	const id = result.id ?? "";
	const existing = (id.length > 0 ? byId.get(id) : undefined) ?? byContent.get(result.content);
	if (existing === undefined) {
		const index = merged.push(result) - 1;
		if (id.length > 0) byId.set(id, index);
		byContent.set(result.content, index);
		return;
	}
	const current = merged[existing];
	if (current && compareRecall(result, current) < 0) merged[existing] = result;
	if (id.length > 0) byId.set(id, existing);
	byContent.set(result.content, existing);
}

function compareRecall(left: RecallResult, right: RecallResult): number {
	return (
		(right.score ?? 0) - (left.score ?? 0) ||
		(right.timestamp ?? "").localeCompare(left.timestamp ?? "") ||
		left.content.localeCompare(right.content)
	);
}

function tokenizeBank(bank: string): string[] {
	return [...new Set(bank.toLowerCase().match(/[a-z0-9]+/g) ?? [])];
}

function escapeRegExp(text: string): string {
	return text.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

/**
 * In per-project-tagged scoping, project-bank tokens in a query can hide broad shared memories from
 * the shared bank. This strips them for one extra pass over the shared bank.
 */
function sharedFallbackQuery(query: string, projectBankName: string, sharedBank: string | undefined): string | undefined {
	if (!sharedBank || projectBankName === sharedBank) return undefined;
	const tokens = tokenizeBank(projectBankName);
	if (tokens.length === 0) return undefined;
	let broadened = query;
	if (tokens.length >= 2) {
		const phrase = tokens.map(escapeRegExp).join("[\\s_-]+");
		broadened = broadened.replace(new RegExp(`\\b${phrase}\\b`, "gi"), " ");
	}
	for (const token of tokens) broadened = broadened.replace(new RegExp(`\\b${escapeRegExp(token)}\\b`, "gi"), " ");
	broadened = broadened
		.replace(/\s+([?!.,;:])/g, "$1")
		.replace(/\b(and|or)\s*([?!.,;:]|$)/gi, "$2")
		.replace(/\s{2,}/g, " ")
		.trim();
	const normalized = broadened.toLowerCase().replace(/[^a-z0-9]+/g, " ").trim();
	const original = query.toLowerCase().replace(/[^a-z0-9]+/g, " ").trim();
	if (normalized.length === 0 || normalized === original) return undefined;
	return broadened;
}

/** Recall across all scoped banks, merged, de-duplicated and capped to `recallLimit`. */
export async function collectRecall(store: Store, query: string): Promise<RecallResult[]> {
	const merged: RecallResult[] = [];
	const byId = new Map<string, number>();
	const byContent = new Map<string, number>();
	const fallback = sharedFallbackQuery(query, store.retain.bank, store.global?.bank);
	const failures: unknown[] = [];
	let succeeded = 0;
	for (const target of store.recall) {
		const queries = target.bank === store.global?.bank && fallback ? [query, fallback] : [query];
		let ok = false;
		try {
			for (const q of queries) {
				const results = await target.memory.recallEnhanced(q, store.cfg.recallLimit, {
					includeFacts: true,
					channelId: target.bank,
				});
				ok = true;
				for (const result of results) mergeRecall(merged, byId, byContent, result);
			}
		} catch (e) {
			failures.push(e);
			log(`recall failed for bank ${target.bank}: ${String(e)}`);
		}
		if (ok) succeeded++;
	}
	if (succeeded === 0 && failures.length > 0) throw failures[0];
	merged.sort(compareRecall);
	return merged.slice(0, store.cfg.recallLimit);
}

