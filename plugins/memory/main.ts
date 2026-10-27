// mu memory plugin: mnemopi recall, retention and the recall/retain tools over JSON-RPC (see plugin.toml).
//   main.ts                          one session on stdio (development; `bun run main.ts`)
//   main.ts daemon --socket <path>   shared daemon serving many sessions (compiled as `mu-memory-daemon`)
import { runDaemon } from "./daemon";
import { serve } from "./rpc";
import { MemorySession } from "./session";

const args = process.argv.slice(2);
if (args[0] === "daemon") {
	const at = args.indexOf("--socket");
	const socketPath = at >= 0 ? args[at + 1] : undefined;
	if (!socketPath) throw new Error("usage: main.ts daemon --socket <path>");
	const idleSecs = Number(process.env.MU_MEMORY_IDLE_SECS);
	await runDaemon(socketPath, (Number.isFinite(idleSecs) && idleSecs >= 0 ? idleSecs : 600) * 1000);
} else {
	const session = new MemorySession();
	await serve(session.handle);
	await session.endSession();
}
